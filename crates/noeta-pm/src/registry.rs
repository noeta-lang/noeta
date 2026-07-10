//! The package **registry index** (package-manager P2.5).
//!
//! A registry is an *index*, not a code store (a locked user decision): it maps a package identity +
//! version to the **git coordinates** (URL + tag) where that release's source lives. Resolving a
//! registry dependency (`webclient = { version = "^1.2", package = "guzzle/http" }`) means asking the
//! index for the published versions of `guzzle/http`, picking the highest that satisfies the
//! requirement, and then fetching its git coordinates through the same path a direct `git` dependency
//! takes. So the registry adds a *name→coordinates* lookup in front of the existing git machinery; it
//! never hosts or serves source.
//!
//! [`Index`] is the contract. The real registry is an HTTP service hosted separately (Cloudflare
//! Workers + KV/D1) and is out of scope here; this module ships [`LocalIndex`], a file-backed index
//! used offline and by tests, plus [`resolve_coords`], the pure selection step. The `noeta publish`
//! verb writes a release into the configured index via [`Index::publish`].
//!
//! **Scope note (surfaced, not hidden).** Version *selection* here is per-dependency: [`resolve_coords`]
//! picks the highest published version matching one requirement. Joint constraint solving across
//! several registry dependencies that share a transitive package — PubGrub backtracking over real
//! version *ranges* — is wired-ready through [`crate::resolve`] but not yet driven from the registry
//! path (git/path pins are exact, so the graph walk selects greedily and reports a conflict rather
//! than backtracking). That depth arrives with the hosted registry, which serves per-version
//! dependency metadata cheaply enough to feed the resolver the whole candidate set.

use std::path::PathBuf;

use semver::{Version, VersionReq};

/// The git coordinates a registry release resolves to (package-manager P2.5). The **commit SHA** the
/// tag resolved to at publish time is pinned here too (Phase 4, S2): the index — not just the
/// lockfile — is authoritative on "this version = this commit", so a *first* registry resolve fetches
/// by the pinned SHA rather than trusting whatever the tag currently points at, and a moved tag is
/// caught against the index. Immutability keys on the whole coordinate, so a tag that moves to a new
/// SHA can't silently replace a published version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCoords {
    pub url: String,
    pub tag: String,
    pub sha: String,
}

/// The registry index contract: look up a package's published versions (each with its git
/// coordinates), and record a new release. Implemented by [`LocalIndex`] here; the hosted service
/// implements the same shape over HTTP.
pub trait Index {
    /// Every published `(version, git coordinates)` of `name` (`company/package`). An unknown package
    /// yields an empty list; an `Err` is a real lookup failure (a corrupt/unreadable index).
    fn versions(&self, name: &str) -> Result<Vec<(Version, GitCoords)>, String>;

    /// Record `name`@`version` → `coords` (the `noeta publish` write path). Re-publishing the same
    /// version with identical coordinates is idempotent; a *different* coordinate for an existing
    /// version is rejected (a published release is immutable).
    fn publish(&self, name: &str, version: &Version, coords: &GitCoords) -> Result<(), String>;
}

/// Resolve a registry requirement to a concrete release: the **highest published version** of `name`
/// satisfying `req`. Errors when the package is unknown or no published version matches (with the
/// available versions listed, matching the project's diagnostic bar).
pub fn resolve_coords(
    index: &dyn Index,
    name: &str,
    req: &VersionReq,
) -> Result<(Version, GitCoords), String> {
    let mut versions = index.versions(name)?;
    if versions.is_empty() {
        return Err(format!("registry has no package `{name}`"));
    }
    // Highest first, so the first match is the selection.
    versions.sort_by(|a, b| b.0.cmp(&a.0));
    versions
        .iter()
        .find(|(v, _)| req.matches(v))
        .cloned()
        .ok_or_else(|| {
            let available: Vec<String> = versions.iter().map(|(v, _)| v.to_string()).collect();
            format!(
                "no version of `{name}` matches `{req}` (published: {})",
                available.join(", ")
            )
        })
}

/// A file-backed [`Index`] (package-manager P2.5): one TOML file per package under a directory, used
/// offline and in tests. Located at `NOETA_REGISTRY_DIR` if set, else `<cache>/registry`. The hosted
/// registry replaces this with an HTTP client of the same [`Index`] shape.
#[derive(Debug)]
pub struct LocalIndex {
    dir: PathBuf,
}

impl LocalIndex {
    /// Open the configured local index, creating its directory. `NOETA_REGISTRY_DIR` overrides the
    /// default `<cache>/registry`. Errors when no location can be resolved or created.
    pub fn open() -> Result<LocalIndex, String> {
        let dir = match std::env::var_os("NOETA_REGISTRY_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => noeta_cache::Cache::locate()
                .ok_or_else(|| {
                    "cannot locate a registry directory (set NOETA_REGISTRY_DIR or HOME)"
                        .to_string()
                })?
                .join("registry"),
        };
        Self::open_at(dir)
    }

    /// Open an index at an explicit directory (tests / an override).
    pub fn open_at(dir: impl Into<PathBuf>) -> Result<LocalIndex, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("cannot create registry dir `{}`: {err}", dir.display()))?;
        Ok(LocalIndex { dir })
    }

    /// The index file for `name` — the `company/package` slash is flattened so it is one file, not a
    /// nested directory (`guzzle/http` → `guzzle__http.toml`).
    fn file_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.toml", name.replace('/', "__")))
    }
}

impl Index for LocalIndex {
    fn versions(&self, name: &str) -> Result<Vec<(Version, GitCoords)>, String> {
        let path = self.file_for(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new()); // unknown package
        };
        let table: toml::Table = text
            .parse()
            .map_err(|err| format!("corrupt registry entry `{}`: {err}", path.display()))?;
        let mut out = Vec::new();
        if let Some(entries) = table.get("version").and_then(|v| v.as_array()) {
            for entry in entries {
                let Some(t) = entry.as_table() else { continue };
                let get = |k: &str| t.get(k).and_then(|v| v.as_str());
                if let (Some(ver), Some(url), Some(tag), Some(sha)) =
                    (get("version"), get("url"), get("tag"), get("sha"))
                    && let Ok(version) = Version::parse(ver)
                {
                    out.push((
                        version,
                        GitCoords {
                            url: url.to_string(),
                            tag: tag.to_string(),
                            sha: sha.to_string(),
                        },
                    ));
                }
            }
        }
        Ok(out)
    }

    fn publish(&self, name: &str, version: &Version, coords: &GitCoords) -> Result<(), String> {
        let mut versions = self.versions(name)?;
        if let Some((_, existing)) = versions.iter().find(|(v, _)| v == version) {
            if existing == coords {
                return Ok(()); // idempotent re-publish
            }
            return Err(format!(
                "`{name}`@{version} is already published at {}#{} — a release is immutable",
                existing.url, existing.tag
            ));
        }
        versions.push((version.clone(), coords.clone()));
        versions.sort_by(|a, b| a.0.cmp(&b.0));

        let mut text = String::from("# noeta registry index entry (generated).\n");
        for (v, c) in &versions {
            text.push_str("\n[[version]]\n");
            text.push_str(&format!("version = {}\n", quote(&v.to_string())));
            text.push_str(&format!("url = {}\n", quote(&c.url)));
            text.push_str(&format!("tag = {}\n", quote(&c.tag)));
            text.push_str(&format!("sha = {}\n", quote(&c.sha)));
        }
        std::fs::write(self.file_for(name), text)
            .map_err(|err| format!("cannot write registry entry for `{name}`: {err}"))
    }
}

/// Minimal TOML basic-string quoting for the values we emit.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(name: &str) -> LocalIndex {
        let dir = std::env::temp_dir().join(format!("noeta_registry_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        LocalIndex::open_at(dir).unwrap()
    }

    fn coords(tag: &str) -> GitCoords {
        GitCoords {
            url: "https://example.com/guzzle/http".to_string(),
            tag: tag.to_string(),
            sha: format!("{tag}-sha"),
        }
    }

    #[test]
    fn publish_then_resolve_picks_highest_match() {
        let index = mem("pick_highest");
        index
            .publish("guzzle/http", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        index
            .publish("guzzle/http", &Version::new(1, 4, 0), &coords("v1.4.0"))
            .unwrap();
        index
            .publish("guzzle/http", &Version::new(2, 0, 0), &coords("v2.0.0"))
            .unwrap();

        let (version, c) =
            resolve_coords(&index, "guzzle/http", &VersionReq::parse("^1.0").unwrap()).unwrap();
        assert_eq!(version, Version::new(1, 4, 0)); // highest in ^1
        assert_eq!(c.tag, "v1.4.0");
    }

    #[test]
    fn resolve_reports_no_match_and_unknown() {
        let index = mem("no_match");
        index
            .publish("guzzle/http", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        let err =
            resolve_coords(&index, "guzzle/http", &VersionReq::parse("^2").unwrap()).unwrap_err();
        assert!(err.contains("no version"), "{err}");
        let err = resolve_coords(&index, "who/dis", &VersionReq::parse("^1").unwrap()).unwrap_err();
        assert!(err.contains("no package"), "{err}");
    }

    #[test]
    fn republishing_a_version_with_new_coords_is_rejected() {
        let index = mem("republish");
        index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        // Same coords: idempotent.
        index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v1.0.0"))
            .unwrap();
        // Different coords for a published version: rejected (immutable).
        let err = index
            .publish("a/b", &Version::new(1, 0, 0), &coords("v9.9.9"))
            .unwrap_err();
        assert!(err.contains("immutable"), "{err}");
    }
}
