//! `noeta.lock` — the reproducible dependency pin (package-manager P2.4c).
//!
//! After the graph walk ([`crate::graph`]) resolves the dependency graph, the resolved coordinates
//! are written here next to `noeta.toml`: each package's identity, version, source, and content hash.
//! A git package additionally pins the **commit SHA** its tag resolved to. On a later build the lock
//! is read back so a git dependency is fetched **by its pinned SHA** ([`crate::git::fetch_pinned`]) —
//! which, if the tree is already in the store, touches the network *not at all* (offline), and
//! otherwise verifies the tag still points at the pinned commit (reproducibility). The lock is a
//! generated file, meant to be committed; the walk remains the source of truth and refreshes it.
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

/// The **pinned trust root** of a scope (`company`), recorded trust-on-first-use in `noeta.lock`.
/// Two roots exist (Phase 4 #2 / Phase 5) and the pin remembers *which* — that memory is the
/// downgrade defense: a scope pinned [`ScopeTrust::Keyless`] refuses a later key-signed or
/// unsigned release, so a registry compromise can't quietly step a scope down to a weaker root.
/// This type is deliberately crypto-free (plain strings): the lock layer — like the LSP — reasons
/// about trust *shapes* without linking any verification stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTrust {
    /// The key root: the scope's registered Ed25519 public key (hex). Signatures verify against
    /// exactly this pinned key; a registry later serving a different key is rejected.
    Key(String),
    /// The keyless root: the OIDC identity (issuer + certificate SAN) that signs this scope's
    /// releases via Sigstore. Bundles must prove exactly this identity.
    Keyless { issuer: String, identity: String },
}

/// A read lockfile: the pins a build consults to reproduce (package-manager P2.4c). Missing or
/// unreadable → [`Lock::empty`] (the walk then resolves from scratch).
#[derive(Debug, Default)]
pub struct Lock {
    /// `(git url, tag)` → pinned commit SHA.
    git_pins: BTreeMap<(String, String), String>,
    /// package identity → content hash (integrity check for immutable git sources).
    hashes: BTreeMap<String, String>,
    /// scope (`company`) → **pinned** trust root, trust-on-first-use (Phase 4 #2 / Phase 5): once
    /// a scope's root is recorded here, a later registry serving a different key, a different
    /// keyless identity, or a *weaker root* (keyless → key/unsigned) is rejected — so a registry
    /// compromised *after* first use can't forge releases or downgrade a scope's trust.
    scope_trust: BTreeMap<String, ScopeTrust>,
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
                }
            }
        }
        if let Some(scopes) = table.get("scope").and_then(|v| v.as_array()) {
            for entry in scopes {
                let Some(s) = entry.as_table() else { continue };
                let get = |k: &str| s.get(k).and_then(|v| v.as_str());
                let Some(scope) = get("name") else { continue };
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
                lock.scope_trust.insert(scope.to_string(), trust);
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

    /// The pinned trust root for `scope`, if the lock records one (provenance TOFU, Phase 4 #2 /
    /// Phase 5).
    pub fn scope_trust(&self, scope: &str) -> Option<&ScopeTrust> {
        self.scope_trust.get(scope)
    }
}

/// Serialize the resolved packages + pinned scope trust to `noeta.lock` text (deterministic, sorted).
fn render(locked: &[LockedPackage], scope_trust: &BTreeMap<String, ScopeTrust>) -> String {
    let mut sorted: Vec<&LockedPackage> = locked.iter().collect();
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
) -> io::Result<()> {
    let path = dir.join(LOCK_NAME);
    let text = render(locked, scope_trust);
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
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
        }
    }

    #[test]
    fn round_trips_pins_and_hashes() {
        let dir = std::env::temp_dir().join("noeta_lock_test_roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut scope_trust = BTreeMap::new();
        scope_trust.insert("acme".to_string(), ScopeTrust::Key("b".repeat(64)));
        write(&dir, &[git_pkg(), path_pkg()], &scope_trust).unwrap();

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
        let dir = std::env::temp_dir().join("noeta_lock_test_keyless_pin");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pin = ScopeTrust::Keyless {
            issuer: "https://token.actions.githubusercontent.com".to_string(),
            identity:
                "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main"
                    .to_string(),
        };
        let mut scope_trust = BTreeMap::new();
        scope_trust.insert("acme".to_string(), pin.clone());
        // A second scope stays on the key root — the two coexist in one lock.
        scope_trust.insert("legacy".to_string(), ScopeTrust::Key("c".repeat(64)));
        write(&dir, &[git_pkg()], &scope_trust).unwrap();

        let lock = Lock::read(&dir);
        assert_eq!(lock.scope_trust("acme"), Some(&pin));
        assert_eq!(
            lock.scope_trust("legacy"),
            Some(&ScopeTrust::Key("c".repeat(64)))
        );
    }

    #[test]
    fn rewrite_is_skipped_when_unchanged() {
        let dir = std::env::temp_dir().join("noeta_lock_test_nochurn");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, &[git_pkg()], &BTreeMap::new()).unwrap();
        let first = std::fs::metadata(dir.join(LOCK_NAME))
            .unwrap()
            .modified()
            .unwrap();
        // A no-op write must not touch the file (same content).
        write(&dir, &[git_pkg()], &BTreeMap::new()).unwrap();
        let second = std::fs::metadata(dir.join(LOCK_NAME))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second, "an unchanged lock must not be rewritten");
    }

    #[test]
    fn a_missing_or_bad_lock_reads_empty() {
        let dir = std::env::temp_dir().join("noeta_lock_test_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
