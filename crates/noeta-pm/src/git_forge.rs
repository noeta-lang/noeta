//! A **git forge as a registry** (private-registries arc, S3) — resolve packages directly from a
//! GitHub org (or any git host) instead of the hosted index service. The convention is Go-module-like:
//! a package `company/pkg` routed here lives at `<host>/<org>/<pkg>`, its **published versions are the
//! semver git tags** (`v1.2.3`), and each version's dependencies come from the `noeta.toml` at that
//! tag. Nothing is stored server-side: the "registry" is just the org's repos + tags.
//!
//! Because it implements the same [`crate::registry::Index`] trait as the hosted index, the resolver
//! treats a git-forge package identically — the `GitCoords` it returns flow through the same fetch path
//! a direct `git` dependency takes. Publishing is `git tag && git push` (so [`Index::publish`] is
//! intentionally unsupported here).
//!
//! Private repos authenticate via git itself: ambient credentials (a helper / `gh auth` / SSH) by
//! default, or `NOETA_GITHUB_TOKEN` as a CI override — see [`crate::git_auth`]. This type holds no
//! credential; auth is applied per git-invocation.

use std::path::{Path, PathBuf};
use std::process::Command;

use semver::Version;

use crate::registry::{Dep, GitCoords, Index, Release};

/// A git-forge registry over one org. Version discovery + per-tag manifests come from a cached bare
/// clone of each repo (so tags and `noeta.toml@tag` are read with plain local git). Authentication for
/// private repos is applied per git-invocation from the environment (see [`crate::git_auth`]) — this
/// type holds no credential.
#[derive(Debug)]
pub struct GitForgeIndex {
    /// The org (GitHub org / user) whose repos back this registry.
    org: String,
    /// The host base for clone URLs — `https://github.com` in production, a local path in tests
    /// (`NOETA_GITHUB_BASE`).
    base: String,
    /// Where bare clones are cached (one per repo).
    cache_dir: PathBuf,
}

impl GitForgeIndex {
    /// Open the GitHub org `org` as a registry, reading configuration from the environment:
    /// `NOETA_GITHUB_BASE` (default `https://github.com`). Private-repo auth is separate (ambient git
    /// credentials, or `NOETA_GITHUB_TOKEN` — see [`crate::git_auth`]). Bare clones are cached under the
    /// toolchain cache dir.
    pub fn github(org: &str) -> Result<GitForgeIndex, String> {
        let base =
            std::env::var("NOETA_GITHUB_BASE").unwrap_or_else(|_| "https://github.com".to_string());
        let cache_dir = match std::env::var_os("NOETA_GIT_FORGE_CACHE") {
            Some(dir) => PathBuf::from(dir),
            None => noeta_cache::Cache::locate()
                .ok_or("cannot locate a cache directory for git-forge registries (set HOME)")?
                .join("git-forge"),
        };
        Ok(GitForgeIndex::new(org, base, cache_dir))
    }

    /// Construct with explicit configuration (used by tests to point at a local repo host).
    pub fn new(
        org: impl Into<String>,
        base: impl Into<String>,
        cache_dir: impl Into<PathBuf>,
    ) -> GitForgeIndex {
        GitForgeIndex {
            org: org.into(),
            base: base.into(),
            cache_dir: cache_dir.into(),
        }
    }

    /// The clone URL for a repo in this org.
    fn repo_url(&self, package: &str) -> String {
        format!(
            "{}/{}/{}",
            self.base.trim_end_matches('/'),
            self.org,
            package
        )
    }

    /// The cached bare-clone path for a repo.
    fn bare_path(&self, package: &str) -> PathBuf {
        self.cache_dir
            .join(&self.org)
            .join(format!("{package}.git"))
    }

    /// Ensure an up-to-date bare clone of the repo exists in the cache; return its path. A first call
    /// clones; a later call refreshes tags. A clone failure (the repo doesn't exist, or is private and
    /// we're unauthenticated) surfaces as the error — which is exactly "no such package here".
    fn ensure_clone(&self, package: &str) -> Result<PathBuf, String> {
        let bare = self.bare_path(package);
        let url = self.repo_url(package);
        if bare.join("HEAD").exists() {
            // Refresh from the URL directly (not a named remote), so it works regardless of how the
            // bare clone configured `origin`.
            git(&[
                "-C",
                path_str(&bare)?,
                "fetch",
                "--tags",
                "--force",
                &url,
                "refs/tags/*:refs/tags/*",
            ])
            .map_err(|err| format!("refreshing `{}/{package}`: {err}", self.org))?;
        } else {
            if let Some(parent) = bare.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("cannot create the git-forge cache: {err}"))?;
            }
            git(&["clone", "--bare", "--quiet", &url, path_str(&bare)?]).map_err(|err| {
                format!(
                    "cannot access `{}/{package}` at {url} — the repo may not exist, or be private \
                     and require authentication: {err}",
                    self.org
                )
            })?;
        }
        Ok(bare)
    }
}

impl Index for GitForgeIndex {
    fn releases(&self, name: &str) -> Result<Vec<Release>, String> {
        let package = name
            .split('/')
            .nth(1)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| format!("`{name}` is not a `company/package` identity"))?;
        let bare = self.ensure_clone(package)?;
        let bare_str = path_str(&bare)?;

        let tag_list = git(&["-C", bare_str, "tag", "--list", "v*"])?;
        let mut releases = Vec::new();
        for tag in tag_list.lines().map(str::trim).filter(|t| !t.is_empty()) {
            // A version tag is `v<semver>`; anything else (a non-release tag) is skipped.
            let Some(version) = tag.strip_prefix('v').and_then(|v| Version::parse(v).ok()) else {
                continue;
            };
            // The commit the tag resolves to (peeling an annotated tag to its commit).
            let sha = git(&["-C", bare_str, "rev-list", "-n", "1", tag])?
                .trim()
                .to_string();
            if sha.is_empty() {
                continue;
            }
            // The version's dependency edges come from its `noeta.toml`. A tag with no manifest (or an
            // unparseable one) isn't a valid package release — skip it rather than fail the listing.
            let Ok(manifest_text) = git(&["-C", bare_str, "show", &format!("{tag}:noeta.toml")])
            else {
                continue;
            };
            let Ok(deps) = registry_deps(&manifest_text) else {
                continue;
            };
            releases.push(Release {
                version,
                coords: GitCoords {
                    url: self.repo_url(package),
                    tag: tag.to_string(),
                    sha,
                },
                deps,
                signature: None,
                bundle: None,
            });
        }
        Ok(releases)
    }

    fn publish(&self, _name: &str, _release: &Release) -> Result<(), String> {
        Err(
            "a GitHub-org registry has no publish endpoint — publish by pushing a semver tag \
             (`git tag v1.2.3 && git push --tags`)"
                .to_string(),
        )
    }

    // scope_key defaults to None: a git-forge registry serves no provenance keys (its trust model is
    // the host's access control + signed commits/tags, not a registry-registered key).
}

/// Extract a published package's **registry** dependency edges from its `noeta.toml` — the only edges a
/// resolver needs from the index. Path/git edges in a published package are ignored (a published
/// package depends on other packages by registry).
fn registry_deps(manifest_text: &str) -> Result<Vec<Dep>, String> {
    let manifest = crate::manifest::Manifest::parse(manifest_text)?;
    let mut deps = Vec::new();
    for dep in manifest.dependencies().values() {
        if let crate::manifest::Dependency::Registry {
            package: Some(pkg),
            req,
        } = dep
        {
            deps.push(Dep {
                package: format!("{}/{}", pkg.company, pkg.package),
                req: req.clone(),
            });
        }
    }
    Ok(deps)
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("path `{}` is not valid UTF-8", p.display()))
}

/// Run `git` with `args`, returning trimmed stdout or an error built from stderr. Token auth
/// (private-registries S5) is prepended so private-repo version discovery authenticates; empty when no
/// `NOETA_GITHUB_TOKEN`, so git uses ambient credentials.
fn git(args: &[&str]) -> Result<String, String> {
    let auth = crate::git_auth::git_auth_args();
    let output = Command::new("git")
        .args(auth.iter().map(String::as_str))
        .args(args)
        .output()
        .map_err(|err| format!("cannot run `git` (is it installed and on PATH?): {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Run git in `dir`, panicking on failure (test setup).
    fn setup_git(dir: &Path, args: &[&str]) {
        let mut a = vec!["-C", dir.to_str().unwrap()];
        a.extend_from_slice(args);
        let out = Command::new("git").args(&a).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Lay out a repo `<host>/<org>/<package>` with a manifest + a couple of tagged releases; the
    /// second version declares a registry dependency (to prove dep extraction).
    fn make_repo(host: &Path, org: &str, package: &str) {
        let repo = host.join(org).join(package);
        std::fs::create_dir_all(&repo).unwrap();
        setup_git(&repo, &["init", "-q", "-b", "main"]);
        setup_git(&repo, &["config", "user.email", "t@t.test"]);
        setup_git(&repo, &["config", "user.name", "T"]);
        std::fs::write(
            repo.join("lib.noe"),
            "namespace thing.core;\npub fn go(): int { return 1; }\n",
        )
        .unwrap();

        std::fs::write(
            repo.join("noeta.toml"),
            format!("[package]\nname = \"{org}/{package}\"\nversion = \"1.0.0\"\n"),
        )
        .unwrap();
        setup_git(&repo, &["add", "."]);
        setup_git(&repo, &["commit", "-q", "-m", "v1"]);
        setup_git(&repo, &["tag", "v1.0.0"]);

        std::fs::write(
            repo.join("noeta.toml"),
            format!(
                "[package]\nname = \"{org}/{package}\"\nversion = \"1.1.0\"\n\
                 [dependencies]\nother = {{ version = \"^2.0\", package = \"{org}/other\" }}\n"
            ),
        )
        .unwrap();
        setup_git(&repo, &["commit", "-q", "-am", "v1.1"]);
        setup_git(&repo, &["tag", "v1.1.0"]);
        // A non-semver tag must be ignored.
        setup_git(&repo, &["tag", "nightly"]);
    }

    #[test]
    fn resolves_versions_coords_and_deps_from_tags() {
        if !git_available() {
            return;
        }
        let tmp = std::env::temp_dir().join("noeta_git_forge_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let host = tmp.join("host");
        let cache = tmp.join("cache");
        make_repo(&host, "acme", "thing");

        let idx = GitForgeIndex::new("acme", host.to_str().unwrap(), cache.clone());
        // Called twice to exercise both the initial clone and the refresh path.
        let _ = idx.releases("acme/thing").unwrap();
        let mut releases = idx.releases("acme/thing").unwrap();
        releases.sort_by(|a, b| a.version.cmp(&b.version));

        let versions: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["1.0.0", "1.1.0"]); // `nightly` skipped

        let v11 = releases
            .iter()
            .find(|r| r.version == Version::new(1, 1, 0))
            .unwrap();
        assert_eq!(v11.coords.tag, "v1.1.0");
        assert_eq!(
            v11.coords.url,
            format!("{}/acme/thing", host.to_str().unwrap())
        );
        assert_eq!(v11.coords.sha.len(), 40);
        // The registry dependency declared at v1.1.0 is surfaced for the resolver.
        assert_eq!(v11.deps.len(), 1);
        assert_eq!(v11.deps[0].package, "acme/other");
        // v1.0.0 declared no dependencies.
        let v10 = releases
            .iter()
            .find(|r| r.version == Version::new(1, 0, 0))
            .unwrap();
        assert!(v10.deps.is_empty());
    }

    #[test]
    fn a_missing_repo_is_a_clear_error() {
        if !git_available() {
            return;
        }
        let tmp = std::env::temp_dir().join("noeta_git_forge_missing");
        let _ = std::fs::remove_dir_all(&tmp);
        let idx = GitForgeIndex::new(
            "acme",
            tmp.join("host").to_str().unwrap(),
            tmp.join("cache"),
        );
        let err = idx.releases("acme/nope").unwrap_err();
        assert!(
            err.contains("may not exist") || err.contains("private"),
            "{err}"
        );
    }

    #[test]
    fn publish_is_unsupported() {
        let idx = GitForgeIndex::new("acme", "https://github.com", std::env::temp_dir());
        let r = Release {
            version: Version::new(1, 0, 0),
            coords: GitCoords {
                url: "u".into(),
                tag: "v1.0.0".into(),
                sha: "s".into(),
            },
            deps: Vec::new(),
            signature: None,
            bundle: None,
        };
        assert!(
            idx.publish("acme/thing", &r)
                .unwrap_err()
                .contains("pushing a semver tag")
        );
    }
}
