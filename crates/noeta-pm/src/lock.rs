//! `noeta.lock` — the reproducible dependency pin (package-manager P2.4c).
//!
//! After the graph walk ([`crate::graph`]) resolves the dependency graph, the resolved coordinates
//! are written here next to `noeta.toml`: each package's identity, version, source, and content hash.
//! A git package additionally pins the **commit SHA** its tag resolved to. On a later build the lock
//! is read back so a git dependency is fetched **by its pinned SHA** ([`crate::git::fetch_pinned`]) —
//! which, if the tree is already in the store, touches the network *not at all* (offline), and
//! otherwise verifies the tag still points at the pinned commit (reproducibility). Registry
//! **selection** is pinned too: when every requirement the local manifests declare is satisfied
//! by the locked versions, the resolver adopts them and never queries the index (the lock fast
//! path in [`crate::graph`]) — so a committed lock reproduces the same versions on any machine,
//! an upstream publish/yank/cooldown can't change an existing build, and a locked build with a
//! warm store is fully offline. The lock is a generated file, meant to be committed; a live
//! resolve (`noeta update`, a new/changed requirement) remains the source of truth and refreshes
//! it.
//!
//! The format is TOML, Cargo-like:
//! ```toml
//! version = 2
//!
//! [[package]]
//! name = "acme/greet"
//! version = "1.0.0"
//! source = "git"
//! url = "https://example.com/acme/greet"
//! tag = "v1.0.0"
//! sha = "…40 hex…"
//! edition = "2026"
//! hash = "…content hash…"
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::graph::{LockedPackage, ResolvedSource};

/// The lockfile name, alongside `noeta.toml`.
pub const LOCK_NAME: &str = "noeta.lock";

/// The lock format version. A lock written by a newer format is ignored (treated as absent) so an
/// older toolchain re-resolves rather than misreading it. Bumped to `2` when the per-package
/// language `edition` was recorded (follow-on F1) — an older toolchain re-resolves rather than
/// reading a lock whose editions it wouldn't understand.
const LOCK_VERSION: i64 = 2;

/// A **pinned trust root**, recorded trust-on-first-use in `noeta.lock`. Two roots exist (Phase 4
/// #2 / Phase 5) and the pin remembers *which* — that memory is the downgrade defense: something
/// pinned [`ScopeTrust::Keyless`] refuses a later key-signed or unsigned release, so a registry
/// compromise can't quietly step it down to a weaker root. This type is deliberately crypto-free
/// (plain strings): the lock layer — like the LSP — reasons about trust *shapes* without linking any
/// verification stack.
///
/// **What a pin is keyed by follows what the root actually identifies.** A registry registers one
/// signing key per *scope*, so [`ScopeTrust::Key`] pins the scope (`para`). A keyless certificate
/// certifies the CI workflow that published the release — which lives in the *package's own*
/// repository (`…/para-html/.github/workflows/release.yml`) — so [`ScopeTrust::Keyless`] pins the
/// package identity (`para/html`). Pinning keyless at the scope would make a scope's second package
/// unresolvable: `para/aether` and `para/html` release from different repos, so no single identity
/// can match both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTrust {
    /// The key root: the scope's registered Ed25519 public key (hex). Signatures verify against
    /// exactly this pinned key; a registry later serving a different key is rejected.
    Key(String),
    /// The keyless root: the OIDC identity (issuer + certificate SAN) that signs this scope's
    /// releases via Sigstore. Bundles must prove exactly this identity.
    Keyless { issuer: String, identity: String },
}

/// The **pinned transparency-log head** (namespace-protection #1, TLog), recorded trust-on-first-use.
/// The log is global to the registry (not per-scope), so the lock keeps a single one: the log's public
/// key plus the last checkpoint (tree size + root) verified. On a later resolve the served checkpoint
/// must be signed by this same key and be an append-only extension of this size/root — so a registry
/// can't rewrite history or equivocate after first use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTrust {
    pub public_key: String,
    pub tree_size: u64,
    pub root_hash: String,
}

/// The **pinned advisory-feed head** (namespace-protection #1, advisory feed), recorded
/// trust-on-first-use. The advisory feed is global to the registry (like the log), so the lock keeps a
/// single pin: the feed's public key plus the last head (`count` + `digest`) verified. On a later audit
/// the served head must be signed by this same key; a `count` that regressed below the pinned one is a
/// rollback (an advisory was dropped) and is surfaced. Crypto-free strings (the lock reasons about
/// trust *shapes*, like [`ScopeTrust`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisoryTrust {
    pub public_key: String,
    pub count: u64,
    pub digest: String,
}

/// A read lockfile: the pins a build consults to reproduce (package-manager P2.4c). Missing or
/// unreadable → [`Lock::empty`] (the walk then resolves from scratch).
#[derive(Debug, Default)]
pub struct Lock {
    /// `(git url, tag)` → pinned commit SHA.
    git_pins: BTreeMap<(String, String), String>,
    /// package identity → its pinned **version** — what makes the lock an actual pin for registry
    /// selection: when every requirement the local manifests declare is satisfied by these
    /// versions, the resolver adopts them and never queries the index (audit: `noeta run` was
    /// effectively `cargo update` on every invocation). Registry releases are immutable at
    /// `(identity, version)`, so a set that was mutually consistent when locked stays consistent.
    versions: BTreeMap<String, semver::Version>,
    /// package identity → its pinned `(url, tag, sha)` — a registry-resolved release's git
    /// coordinates, so a lock-hit build **materializes without the index** (offline when the
    /// store already holds the tree). Only tag-shaped entries qualify (a published release is
    /// always a tag; branch/HEAD entries are direct git deps).
    coords: BTreeMap<String, (String, String, String)>,
    /// package identity → content hash (integrity check for immutable git sources).
    hashes: BTreeMap<String, String>,
    /// package identity → its pinned commit SHA (git sources) — the *previous* commit, so `noeta
    /// update`/`add` can diff old→new and surface a new committer (namespace-protection, committer
    /// signal). Keyed by identity because a version bump changes the tag, so `(url, tag)` won't match.
    shas: BTreeMap<String, String>,
    /// **pinned** trust roots, trust-on-first-use (Phase 4 #2 / Phase 5): once a root is recorded
    /// here, a later registry serving a different key, a different keyless identity, or a *weaker
    /// root* (keyless → key/unsigned) is rejected — so a registry compromised *after* first use
    /// can't forge releases or downgrade trust. Keyed by scope (`para`) for a key root and by
    /// package identity (`para/html`) for a keyless one — see [`ScopeTrust`]. The two never collide:
    /// a package identity always contains a `/`, a scope never does.
    scope_trust: BTreeMap<String, ScopeTrust>,
    /// The pinned transparency-log head (namespace-protection #1, TLog), if recorded.
    log_trust: Option<LogTrust>,
    /// The pinned advisory-feed head (namespace-protection #1, advisory feed), if recorded.
    advisory_trust: Option<AdvisoryTrust>,
}

impl Lock {
    /// An empty lock — no pins, so every dependency is resolved fresh.
    pub fn empty() -> Lock {
        Lock::default()
    }

    /// Read `dir/noeta.lock`, best-effort: a missing file, a parse error, or a version we don't
    /// understand all yield an empty lock (the build then re-resolves and rewrites it).
    pub fn read(dir: &Path) -> Lock {
        let Ok(text) = std::fs::read_to_string(dir.join(LOCK_NAME)) else {
            return Lock::empty();
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            return Lock::empty();
        };
        if table.get("version").and_then(|v| v.as_integer()) != Some(LOCK_VERSION) {
            return Lock::empty();
        }
        let mut lock = Lock::empty();
        if let Some(packages) = table.get("package").and_then(|v| v.as_array()) {
            for entry in packages {
                let Some(pkg) = entry.as_table() else {
                    continue;
                };
                let get = |k: &str| pkg.get(k).and_then(|v| v.as_str());
                let (Some(name), Some(hash)) = (get("name"), get("hash")) else {
                    continue;
                };
                lock.hashes.insert(name.to_string(), hash.to_string());
                // The pinned version (always written; parsed since the lock fast path). An entry
                // whose version doesn't parse simply doesn't pin selection for that package.
                if let Some(v) = get("version").and_then(|v| semver::Version::parse(v).ok()) {
                    lock.versions.insert(name.to_string(), v);
                }
                // Tag-shaped git coordinates double as registry-release coordinates (fast-path
                // materialization without the index).
                if let (Some(url), Some(tag), Some(sha)) = (get("url"), get("tag"), get("sha")) {
                    lock.coords.insert(
                        name.to_string(),
                        (url.to_string(), tag.to_string(), sha.to_string()),
                    );
                }
                // A git package pins its SHA under the ref it was resolved from: a `tag`, a `branch`,
                // or — with neither recorded — the default-branch `HEAD`. The key is rebuilt with the
                // same `GitRef::lock_key` the resolve-time lookup uses, so the pin is found again.
                if let (Some(url), Some(sha)) = (get("url"), get("sha")) {
                    let git_ref = match (get("tag"), get("branch")) {
                        (Some(tag), _) => crate::manifest::GitRef::Tag(tag.to_string()),
                        (None, Some(branch)) => crate::manifest::GitRef::Branch(branch.to_string()),
                        (None, None) => crate::manifest::GitRef::Head,
                    };
                    lock.git_pins
                        .insert((url.to_string(), git_ref.lock_key()), sha.to_string());
                    lock.shas.insert(name.to_string(), sha.to_string());
                }
            }
        }
        if let Some(scopes) = table.get("scope").and_then(|v| v.as_array()) {
            for entry in scopes {
                let Some(s) = entry.as_table() else { continue };
                let get = |k: &str| s.get(k).and_then(|v| v.as_str());
                let Some(name) = get("name") else { continue };
                // The entry's shape says which trust root is pinned: `public_key` = the key root,
                // `issuer` + `identity` = the keyless root. An entry with neither is ignored.
                let trust = match (get("public_key"), get("issuer"), get("identity")) {
                    (Some(key), _, _) => ScopeTrust::Key(key.to_string()),
                    (None, Some(issuer), Some(identity)) => ScopeTrust::Keyless {
                        issuer: issuer.to_string(),
                        identity: identity.to_string(),
                    },
                    _ => continue,
                };
                lock.scope_trust.insert(name.to_string(), trust);
            }
        }
        lock.migrate_legacy_keyless_pins();
        if let Some(l) = table.get("log").and_then(|v| v.as_table()) {
            let get = |k: &str| l.get(k).and_then(|v| v.as_str());
            if let (Some(public_key), Some(size), Some(root_hash)) = (
                get("public_key"),
                l.get("tree_size").and_then(|v| v.as_integer()),
                get("root_hash"),
            ) {
                lock.log_trust = Some(LogTrust {
                    public_key: public_key.to_string(),
                    tree_size: size.max(0) as u64,
                    root_hash: root_hash.to_string(),
                });
            }
        }
        if let Some(a) = table.get("advisory").and_then(|v| v.as_table()) {
            let get = |k: &str| a.get(k).and_then(|v| v.as_str());
            if let (Some(public_key), Some(count), Some(digest)) = (
                get("public_key"),
                a.get("count").and_then(|v| v.as_integer()),
                get("digest"),
            ) {
                lock.advisory_trust = Some(AdvisoryTrust {
                    public_key: public_key.to_string(),
                    count: count.max(0) as u64,
                    digest: digest.to_string(),
                });
            }
        }
        lock
    }

    /// The pinned commit SHA for a git `url` at `git_ref`, if the lock records it.
    pub fn git_pin(&self, url: &str, git_ref: &crate::manifest::GitRef) -> Option<&str> {
        self.git_pins
            .get(&(url.to_string(), git_ref.lock_key()))
            .map(String::as_str)
    }

    /// The recorded content hash for a package identity, if any.
    pub fn content_hash(&self, identity: &str) -> Option<&str> {
        self.hashes.get(identity).map(String::as_str)
    }

    /// The pinned version for a package identity, if the lock records one — what the resolver's
    /// lock fast path adopts instead of querying the index.
    pub fn locked_version(&self, identity: &str) -> Option<&semver::Version> {
        self.versions.get(identity)
    }

    /// Every pinned `identity → version` (the lock fast path adopts the whole set; the walk only
    /// materializes what the manifests actually reference, so a stale extra entry is inert and
    /// drops out on the next lock rewrite).
    pub fn locked_versions(&self) -> impl Iterator<Item = (&String, &semver::Version)> {
        self.versions.iter()
    }

    /// The pinned `(url, tag, sha)` coordinates for a package identity, if the lock records them —
    /// how a lock-hit registry dependency materializes without the index.
    pub fn registry_coords(&self, identity: &str) -> Option<(&str, &str, &str)> {
        self.coords
            .get(identity)
            .map(|(u, t, s)| (u.as_str(), t.as_str(), s.as_str()))
    }

    /// The previously-pinned commit SHA for a package identity (git sources), if the lock records one
    /// — the `since` point for the committer-signal diff on `noeta update`/`add`.
    pub fn git_sha(&self, identity: &str) -> Option<&str> {
        self.shas.get(identity).map(String::as_str)
    }

    /// The pinned trust root recorded under `key` — a scope (`para`) for a key root, a package
    /// identity (`para/html`) for a keyless one (provenance TOFU, Phase 4 #2 / Phase 5).
    pub fn scope_trust(&self, key: &str) -> Option<&ScopeTrust> {
        self.scope_trust.get(key)
    }

    /// The pinned trust root to verify a release of `identity` against: the package's own keyless
    /// pin if it has one, else its scope's key pin. Checking the package first is what lets one
    /// scope hold packages that release from different repositories (see [`ScopeTrust`]).
    pub fn trust_for(&self, identity: &str) -> Option<&ScopeTrust> {
        let scope = identity.split('/').next().unwrap_or(identity);
        self.scope_trust
            .get(identity)
            .or_else(|| self.scope_trust.get(scope))
    }

    /// Re-key a lock written before keyless pins moved from the scope to the package
    /// ([`ScopeTrust`]). A bare-scope keyless entry names an identity that was certified for one
    /// specific package's repository; when the lock records **exactly one** package in that scope,
    /// that package is unambiguously the one it was pinned from, so the pin moves onto it.
    ///
    /// Any other shape — several packages in the scope, or a lock recording none at all — leaves
    /// the entry **exactly where it is**, still enforced through [`Self::trust_for`]'s scope
    /// fallback. A pin is a security control, so an ambiguity about who it belongs to is never
    /// grounds for silently dropping it: that would turn a downgrade the pin exists to catch into
    /// an accepted release. The stale spelling costs nothing — a resolve records per-package pins
    /// for what it resolves, so the entry is gone from the next lock the toolchain writes.
    fn migrate_legacy_keyless_pins(&mut self) {
        let legacy: Vec<String> = self
            .scope_trust
            .iter()
            .filter(|(name, trust)| {
                !name.contains('/') && matches!(trust, ScopeTrust::Keyless { .. })
            })
            .map(|(name, _)| name.clone())
            .collect();
        for scope in legacy {
            let mut in_scope = self
                .versions
                .keys()
                .filter(|identity| identity.split('/').next() == Some(scope.as_str()));
            let (Some(only), None) = (in_scope.next().cloned(), in_scope.next()) else {
                continue;
            };
            if let Some(trust) = self.scope_trust.remove(&scope) {
                self.scope_trust.entry(only).or_insert(trust);
            }
        }
    }

    /// The pinned transparency-log head, if the lock records one (namespace-protection #1, TLog).
    pub fn log_trust(&self) -> Option<&LogTrust> {
        self.log_trust.as_ref()
    }

    /// The pinned advisory-feed head (namespace-protection #1), if the lock records one.
    pub fn advisory_trust(&self) -> Option<&AdvisoryTrust> {
        self.advisory_trust.as_ref()
    }
}

/// Serialize the resolved packages + pinned scope trust + transparency-log head to `noeta.lock` text
/// (deterministic, sorted).
fn render(
    locked: &[LockedPackage],
    scope_trust: &BTreeMap<String, ScopeTrust>,
    log_trust: Option<&LogTrust>,
    advisory_trust: Option<&AdvisoryTrust>,
) -> String {
    // The `[patch]` decision (dev-time path override): a patched identity is NOT written — not
    // even with a marker. The lock is the reproducibility record, and a patched tree is a mutable
    // local override other machines don't have: recording it would make every rebuild chase a
    // content hash that changes on each edit, and a `patched = true` marker would still strand a
    // stale entry when the patch is removed. Omission keeps the file self-consistent in both
    // directions — while the patch is active the identity simply has no pin (the override IS the
    // selection, so `noeta check` stays warning-free), and removing the patch re-pins the identity
    // from its declared source on the next resolve.
    let mut sorted: Vec<&LockedPackage> = locked.iter().filter(|p| !p.patched).collect();
    sorted.sort_by(|a, b| a.identity.cmp(&b.identity));
    let mut out = String::new();
    out.push_str("# This file is generated by noeta; it is meant to be committed. Do not edit.\n");
    out.push_str(&format!("version = {LOCK_VERSION}\n"));
    for pkg in sorted {
        out.push_str("\n[[package]]\n");
        out.push_str(&format!("name = {}\n", quote(&pkg.identity)));
        out.push_str(&format!("version = {}\n", quote(&pkg.version.to_string())));
        match &pkg.source {
            ResolvedSource::Path { path } => {
                out.push_str("source = \"path\"\n");
                out.push_str(&format!("path = {}\n", quote(&path.display().to_string())));
            }
            ResolvedSource::Git { url, git_ref, sha } => {
                out.push_str("source = \"git\"\n");
                out.push_str(&format!("url = {}\n", quote(url)));
                // The ref the SHA was resolved from: a `tag` or `branch` line, or neither for a
                // default-branch HEAD dependency (which `noeta update` re-resolves to a new SHA).
                match git_ref {
                    crate::manifest::GitRef::Tag(tag) => {
                        out.push_str(&format!("tag = {}\n", quote(tag)));
                    }
                    crate::manifest::GitRef::Branch(branch) => {
                        out.push_str(&format!("branch = {}\n", quote(branch)));
                    }
                    crate::manifest::GitRef::Head => {}
                }
                out.push_str(&format!("sha = {}\n", quote(sha)));
            }
        }
        if let Some(native) = &pkg.native {
            out.push_str(&format!("native = {}\n", quote(native)));
        }
        out.push_str(&format!("edition = {}\n", quote(pkg.edition.as_str())));
        out.push_str(&format!("hash = {}\n", quote(&pkg.content_hash)));
    }
    // Pinned scope trust roots (provenance TOFU): once written, a later differing key, differing
    // identity, or weaker root is rejected.
    for (scope, trust) in scope_trust {
        out.push_str("\n[[scope]]\n");
        out.push_str(&format!("name = {}\n", quote(scope)));
        match trust {
            ScopeTrust::Key(key) => {
                out.push_str(&format!("public_key = {}\n", quote(key)));
            }
            ScopeTrust::Keyless { issuer, identity } => {
                out.push_str(&format!("issuer = {}\n", quote(issuer)));
                out.push_str(&format!("identity = {}\n", quote(identity)));
            }
        }
    }
    // The pinned transparency-log head (TLog TOFU): a later checkpoint must be signed by this key and
    // be an append-only extension of this size/root.
    if let Some(log) = log_trust {
        out.push_str("\n[log]\n");
        out.push_str(&format!("public_key = {}\n", quote(&log.public_key)));
        out.push_str(&format!("tree_size = {}\n", log.tree_size));
        out.push_str(&format!("root_hash = {}\n", quote(&log.root_hash)));
    }
    // The pinned advisory-feed head (advisory-feed TOFU): a later head must be signed by this key, and
    // a `count` below this one is a dropped-advisory rollback.
    if let Some(adv) = advisory_trust {
        out.push_str("\n[advisory]\n");
        out.push_str(&format!("public_key = {}\n", quote(&adv.public_key)));
        out.push_str(&format!("count = {}\n", adv.count));
        out.push_str(&format!("digest = {}\n", quote(&adv.digest)));
    }
    out
}

/// Write `dir/noeta.lock` if its content would change (avoiding mtime churn on every run). Atomic
/// (temp file + rename). Best-effort at the call site: a read-only project shouldn't fail a build, so
/// the caller may ignore the error — but the error is returned so a dedicated verb (`noeta update`)
/// can surface it.
pub fn write(
    dir: &Path,
    locked: &[LockedPackage],
    scope_trust: &BTreeMap<String, ScopeTrust>,
    log_trust: Option<&LogTrust>,
    advisory_trust: Option<&AdvisoryTrust>,
) -> io::Result<()> {
    let path = dir.join(LOCK_NAME);
    let text = render(locked, scope_trust, log_trust, advisory_trust);
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        return Ok(()); // unchanged
    }
    let tmp = dir.join(format!(".{LOCK_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, &path)
}

/// Minimal TOML basic-string quoting for the values we emit (identities, versions, SHAs, URLs, and
/// paths). Escapes backslashes and double quotes; the values never contain control characters.
fn quote(s: &str) -> String {
    crate::toml_quote(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use std::path::PathBuf;

    fn git_pkg() -> LockedPackage {
        LockedPackage {
            identity: "acme/greet".to_string(),
            version: Version::new(1, 0, 0),
            content_hash: "deadbeef".to_string(),
            source: ResolvedSource::Git {
                url: "https://example.com/acme/greet".to_string(),
                git_ref: crate::manifest::GitRef::Tag("v1.0.0".to_string()),
                sha: "a".repeat(40),
            },
            native: None,
            edition: crate::edition::Edition::E2026,
            patched: false,
        }
    }

    fn path_pkg() -> LockedPackage {
        LockedPackage {
            identity: "acme/local".to_string(),
            version: Version::new(0, 2, 0),
            content_hash: "cafe".to_string(),
            source: ResolvedSource::Path {
                path: PathBuf::from("../local"),
            },
            native: Some("native".to_string()),
            edition: crate::edition::Edition::E2026,
            patched: false,
        }
    }

    #[test]
    fn round_trips_pins_and_hashes() {
        let dir = crate::test_temp::TempDir::new("roundtrip");
        let mut scope_trust = BTreeMap::new();
        scope_trust.insert("acme".to_string(), ScopeTrust::Key("b".repeat(64)));
        write(&dir, &[git_pkg(), path_pkg()], &scope_trust, None, None).unwrap();

        let lock = Lock::read(&dir);
        assert_eq!(
            lock.git_pin(
                "https://example.com/acme/greet",
                &crate::manifest::GitRef::Tag("v1.0.0".to_string())
            ),
            Some("a".repeat(40).as_str())
        );
        assert_eq!(lock.content_hash("acme/greet"), Some("deadbeef"));
        assert_eq!(lock.content_hash("acme/local"), Some("cafe"));
        // The selection pin (lock fast path): version + registry coordinates read back.
        assert_eq!(
            lock.locked_version("acme/greet"),
            Some(&Version::new(1, 0, 0))
        );
        assert_eq!(
            lock.registry_coords("acme/greet"),
            Some((
                "https://example.com/acme/greet",
                "v1.0.0",
                "a".repeat(40).as_str()
            ))
        );
        // A path package pins a version but no git coordinates.
        assert_eq!(
            lock.locked_version("acme/local"),
            Some(&Version::new(0, 2, 0))
        );
        assert_eq!(lock.registry_coords("acme/local"), None);
        // A path package records no git pin.
        assert_eq!(
            lock.git_pin("../local", &crate::manifest::GitRef::Head),
            None
        );
        // The pinned scope key round-trips (provenance TOFU, Phase 4 #2).
        assert_eq!(
            lock.scope_trust("acme"),
            Some(&ScopeTrust::Key("b".repeat(64)))
        );
        assert_eq!(lock.scope_trust("nobody"), None);

        // The per-package edition is recorded for reproducibility (follow-on F1), under the bumped
        // format version. It is a record field (re-derived from manifests on resolve, like `native`),
        // so it round-trips via the rendered file, not the read model.
        let text = std::fs::read_to_string(dir.join(LOCK_NAME)).unwrap();
        assert!(
            text.contains("version = 2\n"),
            "format version bumped: {text}"
        );
        assert_eq!(
            text.matches("edition = \"2026\"").count(),
            2,
            "both packages record their edition: {text}"
        );
    }

    #[test]
    fn a_keyless_identity_pin_round_trips() {
        let dir = crate::test_temp::TempDir::new("keyless-pin");
        let pin = ScopeTrust::Keyless {
            issuer: "https://token.actions.githubusercontent.com".to_string(),
            identity:
                "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main"
                    .to_string(),
        };
        let mut scope_trust = BTreeMap::new();
        // A keyless pin is keyed by the *package* — the certificate names that package's own
        // release workflow, so a sibling package of the scope carries a different identity.
        scope_trust.insert("acme/greet".to_string(), pin.clone());
        // A scope on the key root keys by the scope — a registry registers one key per scope. The
        // two coexist in one lock, and never collide (a package identity always has a `/`).
        scope_trust.insert("legacy".to_string(), ScopeTrust::Key("c".repeat(64)));
        write(&dir, &[git_pkg()], &scope_trust, None, None).unwrap();

        let lock = Lock::read(&dir);
        assert_eq!(lock.scope_trust("acme/greet"), Some(&pin));
        assert_eq!(
            lock.scope_trust("legacy"),
            Some(&ScopeTrust::Key("c".repeat(64)))
        );
        // `trust_for` is the resolve-time lookup: a package's own keyless pin, else its scope key.
        assert_eq!(lock.trust_for("acme/greet"), Some(&pin));
        assert_eq!(
            lock.trust_for("legacy/thing"),
            Some(&ScopeTrust::Key("c".repeat(64)))
        );
        // A sibling package of the keyless scope is unpinned — it TOFUs its own identity rather
        // than being measured against `acme/greet`'s workflow (which it could never match).
        assert_eq!(lock.trust_for("acme/other"), None);
    }

    #[test]
    fn a_legacy_scope_keyless_pin_migrates_to_its_package() {
        let pin = ScopeTrust::Keyless {
            issuer: "https://token.actions.githubusercontent.com".to_string(),
            identity:
                "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main"
                    .to_string(),
        };
        // A lock written when keyless pinned the scope: one package in `acme`, so the pin
        // unambiguously belongs to it and is carried over intact.
        let dir = crate::test_temp::TempDir::new("legacy-pin-one");
        let mut legacy = BTreeMap::new();
        legacy.insert("acme".to_string(), pin.clone());
        write(&dir, &[git_pkg()], &legacy, None, None).unwrap();
        let lock = Lock::read(&dir);
        assert_eq!(lock.trust_for("acme/greet"), Some(&pin));
        assert_eq!(
            lock.scope_trust("acme"),
            None,
            "the bare scope pin is re-keyed"
        );

        // Two packages in the scope: the old pin could only ever have matched one of them, so it
        // is dropped and each package re-pins (verified afresh) on the next resolve.
        let dir = crate::test_temp::TempDir::new("legacy-pin-many");
        write(&dir, &[git_pkg(), path_pkg()], &legacy, None, None).unwrap();
        let lock = Lock::read(&dir);
        // Ambiguous which package it was pinned from, so it stays a scope entry and keeps being
        // enforced through the scope fallback — dropping a pin would accept the very downgrade it
        // exists to catch. Each package re-pins onto itself on the next resolve.
        assert_eq!(lock.trust_for("acme/greet"), Some(&pin));
        assert_eq!(lock.trust_for("acme/local"), Some(&pin));

        // The same holds for a lock that records the pin but no packages at all.
        let dir = crate::test_temp::TempDir::new("legacy-pin-none");
        std::fs::write(
            dir.join(LOCK_NAME),
            format!(
                "version = {LOCK_VERSION}\n\n[[scope]]\nname = \"acme\"\n\
                 issuer = \"https://token.actions.githubusercontent.com\"\n\
                 identity = \"https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main\"\n"
            ),
        )
        .unwrap();
        assert_eq!(Lock::read(&dir).trust_for("acme/anything"), Some(&pin));
    }

    #[test]
    fn a_transparency_log_head_round_trips() {
        let dir = crate::test_temp::TempDir::new("logtrust");
        let log = LogTrust {
            public_key: "ab".repeat(32),
            tree_size: 42,
            root_hash: "cd".repeat(32),
        };
        write(&dir, &[git_pkg()], &BTreeMap::new(), Some(&log), None).unwrap();
        assert_eq!(Lock::read(&dir).log_trust(), Some(&log));
        // A lock with no `[log]` block reports no pin.
        let dir2 = crate::test_temp::TempDir::new("nolog");
        write(&dir2, &[git_pkg()], &BTreeMap::new(), None, None).unwrap();
        assert_eq!(Lock::read(&dir2).log_trust(), None);
    }

    #[test]
    fn an_advisory_feed_head_round_trips() {
        let dir = crate::test_temp::TempDir::new("advtrust");
        let adv = AdvisoryTrust {
            public_key: "ef".repeat(32),
            count: 7,
            digest: "12".repeat(32),
        };
        write(&dir, &[git_pkg()], &BTreeMap::new(), None, Some(&adv)).unwrap();
        assert_eq!(Lock::read(&dir).advisory_trust(), Some(&adv));
        // The advisory pin coexists with a log pin.
        let log = LogTrust {
            public_key: "ab".repeat(32),
            tree_size: 3,
            root_hash: "cd".repeat(32),
        };
        write(&dir, &[git_pkg()], &BTreeMap::new(), Some(&log), Some(&adv)).unwrap();
        let read = Lock::read(&dir);
        assert_eq!(read.advisory_trust(), Some(&adv));
        assert_eq!(read.log_trust(), Some(&log));
    }

    #[test]
    fn a_patched_package_is_omitted_from_the_lock() {
        // Dev-time path override: the lock records only real, reproducible state — a `[patch]`ed
        // identity is not written at all (no entry, no marker), so removing the patch leaves a
        // self-consistent lock that simply re-pins the identity on the next resolve.
        let dir = crate::test_temp::TempDir::new("patched-omitted");
        let mut patched = path_pkg();
        patched.patched = true;
        write(&dir, &[git_pkg(), patched], &BTreeMap::new(), None, None).unwrap();
        let text = std::fs::read_to_string(dir.join(LOCK_NAME)).unwrap();
        assert!(
            text.contains("acme/greet"),
            "the real pin is written: {text}"
        );
        assert!(
            !text.contains("acme/local") && !text.contains("patched"),
            "the patched identity leaves no trace in the lock: {text}"
        );
        let lock = Lock::read(&dir);
        assert_eq!(lock.locked_version("acme/local"), None);
        assert_eq!(
            lock.locked_version("acme/greet"),
            Some(&Version::new(1, 0, 0))
        );
    }

    #[test]
    fn rewrite_is_skipped_when_unchanged() {
        let dir = crate::test_temp::TempDir::new("nochurn");
        write(&dir, &[git_pkg()], &BTreeMap::new(), None, None).unwrap();
        let first = std::fs::metadata(dir.join(LOCK_NAME))
            .unwrap()
            .modified()
            .unwrap();
        // A no-op write must not touch the file (same content).
        write(&dir, &[git_pkg()], &BTreeMap::new(), None, None).unwrap();
        let second = std::fs::metadata(dir.join(LOCK_NAME))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second, "an unchanged lock must not be rewritten");
    }

    #[test]
    fn a_missing_or_bad_lock_reads_empty() {
        let dir = crate::test_temp::TempDir::new("missing");
        assert!(
            Lock::read(&dir)
                .git_pin("x", &crate::manifest::GitRef::Tag("y".to_string()))
                .is_none()
        );
        // A future version is ignored (re-resolve rather than misread).
        std::fs::write(dir.join(LOCK_NAME), "version = 999\n").unwrap();
        assert!(Lock::read(&dir).content_hash("acme/greet").is_none());
    }
}
